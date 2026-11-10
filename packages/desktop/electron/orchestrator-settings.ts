/**
 * Settings helpers for the Crab Desktop Orchestrator.
 *
 * Two responsibilities:
 *
 * 1. `loadAgentConfig(agentType)` / `saveAgentConfig(agentType, config)`
 *    share per-agent configuration (binary path override, extra CLI args,
 *    env vars) between settings IPC and adapter startup.
 *
 * 2. `migrateSettings()` — one-shot migration from the legacy
 *    per-agent path keys (`orchestratorClaudeCodePath`,
 *    `orchestratorCodexPath`, `orchestratorCustomBinaryPath`) into
 *    the unified `orchestratorAgentConfigs` structure, preserving
 *    the old keys as a safe fallback for downgrade scenarios.
 *
 * The settings file is `~/.crab/desktop/settings.json` — the same
 * file the renderer writes to via IPC handlers in `orchestrator-ipc.ts`.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

/**
 * Resolves the active settings path. Returns
 * `process.env.CRAB_DESKTOP_SETTINGS_PATH` when set (tests use this
 * to redirect to a sandbox), otherwise the default user-level location.
 */
function settingsPath(): string {
  return process.env.CRAB_DESKTOP_SETTINGS_PATH
    ?? path.join(os.homedir(), ".crab", "desktop", "settings.json");
}

/** Per-agent configuration as stored under `orchestratorAgentConfigs[agentType]`. */
export interface StoredAgentConfig {
  binaryPath?: string;
  extraArgs?: string[];
  env?: Record<string, string>;
  enabled?: boolean;
  /** Codex-specific: notification methods to suppress. */
  optOutNotifications?: string[];
}

/**
 * Read settings with literal per-agent keys and an empty agent map when missing or unreadable.
 */
export function readSettings(): Record<string, unknown> & {
  orchestratorAgentConfigs: Record<string, unknown>;
} {
  let settings: Record<string, unknown> = {};
  try {
    const p = settingsPath();
    if (fs.existsSync(p)) settings = JSON.parse(fs.readFileSync(p, "utf-8")) as Record<string, unknown>;
  } catch {
    // Optional saved configuration must not prevent agent startup.
  }
  // Manifest identifiers are literal dictionary keys. An inherited prototype
  // can turn an assignment into a prototype change that JSON never persists.
  const configs = Object.create(null) as Record<string, unknown>;
  Object.assign(configs, settings.orchestratorAgentConfigs);
  return { ...settings, orchestratorAgentConfigs: configs };
}

/** Writes the settings file, creating the parent directory if needed. */
export function writeSettings(settings: Record<string, unknown>): void {
  const p = settingsPath();
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, JSON.stringify(settings, null, 2), "utf-8");
}

/**
 * Loads per-agent config for the given agent type. Returns an empty
 * object when the agent has no saved config yet.
 */
export function loadAgentConfig(agentType: string): StoredAgentConfig {
  const settings = readSettings();
  return (settings.orchestratorAgentConfigs[agentType] ?? {}) as StoredAgentConfig;
}

/** Persist one agent's configuration while retaining unrelated settings. */
export function saveAgentConfig(agentType: string, config: unknown): void {
  const settings = readSettings();
  settings.orchestratorAgentConfigs[agentType] = config;
  writeSettings(settings);
}

/**
 * One-shot migration from legacy per-agent path keys into the
 * unified `orchestratorAgentConfigs` structure. Idempotent — safe to
 * call on every startup. The old keys are intentionally preserved
 * so a downgrade doesn't silently lose the user's configuration.
 */
export function migrateSettings(): void {
  const settings = readSettings();

  if (settings.orchestratorAgentConfigsMigrationV1 === true) {
    return; // already migrated
  }

  const configs = settings.orchestratorAgentConfigs as Record<string, StoredAgentConfig>;
  let touched = false;

  const claudePath = settings.orchestratorClaudeCodePath as string | undefined;
  if (claudePath && !configs["claude-code"]?.binaryPath) {
    configs["claude-code"] = { ...configs["claude-code"], binaryPath: claudePath };
    touched = true;
  }

  const codexPath = settings.orchestratorCodexPath as string | undefined;
  if (codexPath && !configs["codex"]?.binaryPath) {
    configs["codex"] = { ...configs["codex"], binaryPath: codexPath };
    touched = true;
  }

  const customPath = settings.orchestratorCustomBinaryPath as string | undefined;
  if (customPath && !configs["custom"]?.binaryPath) {
    configs["custom"] = { ...configs["custom"], binaryPath: customPath };
    touched = true;
  }

  settings.orchestratorAgentConfigs = configs;
  settings.orchestratorAgentConfigsMigrationV1 = true;

  // Always persist the migration flag so repeated calls on a fresh install
  // short-circuit on the next run.
  try {
    writeSettings(settings);
  } catch {
    // Non-fatal — migration will retry next launch.
  }
  void touched;
}
